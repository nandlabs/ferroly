# `ferroly::rt` — async runtime surface

Ferroly depends on `tokio` internally. The `rt` module re-exports a curated
slice of it so a consumer can spawn tasks, use channels, drive timers, open
sockets and files, and run child processes **without adding `tokio` to their own
`Cargo.toml`** — and adds three small shutdown helpers of Ferroly's own:
[`CancellationToken`](#cancellationtoken), [`TaskTracker`](#tasktracker) and
[`shutdown_signal`](#shutdown_signal).

Enable with the `rt` feature (it pulls in `tokio`):

```toml
ferroly = { version = "0.3", features = ["rt"] }
```

```rust
use ferroly::rt::{self, sync::mpsc, time::Duration};

# async fn demo() {
let (tx, mut rx) = mpsc::channel::<u32>(8);
let worker = rt::spawn(async move {
    tx.send(42).await.ok();
});

rt::select! {
    msg = rx.recv() => { let _ = msg; }
    _ = rt::time::sleep(Duration::from_secs(1)) => {}
}
worker.await.ok();
# }
```

## What works without a direct `tokio` dependency

With only `ferroly` (feature `rt`) in `Cargo.toml` — no `tokio`, not even as a
dev-dependency:

| Item | Works as-is? |
|---|---|
| Every function, type and module listed under [What's re-exported](#whats-re-exported) | yes |
| `select!`, `join!`, `try_join!`, `pin!`, `task_local!` | yes |
| `async_test!`, `async_main!` (Ferroly's own) | yes |
| `CancellationToken`, `TaskTracker`, `shutdown_signal` | yes |
| `#[ferroly::rt::test]` / `#[ferroly::rt::main]` written **bare** | **no** — compile error: `cannot find crate tokio` |
| `#[ferroly::rt::test(crate = "ferroly::rt::__tokio")]` (and the same for `main`) | yes |
| `time::pause` / `advance` / `resume` (paused test clock) | **not available** — see [Not re-exported](#not-re-exported) |

The two attributes are tokio's own procedural macros, re-exported unchanged.
They expand to `::tokio::…` paths, and a re-export cannot change the paths a
procedural macro emits — so the bare form only compiles in a crate that also
depends on `tokio`. The next section shows the two ways around that.

## Entry points and async tests

### Declarative macros (nothing to configure)

`async_test!` and `async_main!` are Ferroly's own `macro_rules!` macros. They
expand to paths inside `ferroly`, so they need no `tokio` dependency and no
arguments:

```rust
ferroly::rt::async_main! {
    async fn main() -> std::io::Result<()> {
        let listener = ferroly::rt::net::TcpListener::bind("127.0.0.1:0").await?;
        println!("listening on {}", listener.local_addr()?);
        Ok(())
    }
}
```

```rust
ferroly::rt::async_test! {
    async fn channel_roundtrip() {
        let (tx, mut rx) = ferroly::rt::sync::mpsc::channel(1);
        tx.send(7u8).await.unwrap();
        assert_eq!(rx.recv().await, Some(7));
    }

    #[should_panic(expected = "boom")]
    async fn panics() {
        panic!("boom");
    }

    async fn with_result() -> Result<(), std::num::ParseIntError> {
        let n: u8 = "7".parse()?;
        assert_eq!(n, 7);
        Ok(())
    }
}
```

| Macro | Runtime | Notes |
|---|---|---|
| `async_test! { async fn a() { … } async fn b() { … } }` | a fresh current-thread runtime per test, I/O and time enabled (tokio's test default) | one or more functions; attributes (`#[ignore]`, `#[should_panic]`) and a return type are carried over; functions take no arguments |
| `async_main! { async fn main() { … } }` | a multi-thread runtime, I/O and time enabled (tokio's `main` default) | exactly one `main`; a return type is carried over |

They do not take tokio's options (`flavor`, `worker_threads`, `start_paused`,
…). For those, use the attribute form below or `runtime::Builder`.

### Attribute macros (all of tokio's options)

`#[ferroly::rt::test]` and `#[ferroly::rt::main]` accept everything
`#[tokio::test]` / `#[tokio::main]` do. In a crate without its own `tokio`
dependency, tell them where tokio lives with the `crate` argument:

```rust
#[ferroly::rt::main(crate = "ferroly::rt::__tokio")]
async fn main() { /* … */ }

#[ferroly::rt::test(crate = "ferroly::rt::__tokio")]
async fn it_works() { /* … */ }

#[ferroly::rt::test(crate = "ferroly::rt::__tokio", flavor = "multi_thread", worker_threads = 2)]
async fn on_two_workers() { /* … */ }
```

`ferroly::rt::__tokio` is the `tokio` crate, re-exported for exactly this
purpose; it is hidden from the generated docs but is a supported path. If you
renamed the `ferroly` dependency in your `Cargo.toml`, use your name in the
string. In a crate that *does* depend on `tokio`, the bare attributes work too.

### By hand

A binary that owns its reactor can also build it explicitly:

```rust
use ferroly::rt::runtime;

fn main() {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async { /* serve */ });
}
```

## What's re-exported

| Area | Path | Items |
|---|---|---|
| Tasks | `ferroly::rt` | `spawn`, `spawn_blocking`, `spawn_local`, `block_in_place`, `yield_now`, `JoinHandle`, `JoinSet`, `JoinError`, `AbortHandle`, `LocalSet`, `LocalKey` |
| Macros | `ferroly::rt` | `select!`, `join!`, `try_join!`, `pin!`, `task_local!`; `#[test]` / `#[main]` (see above); `async_test!`, `async_main!` |
| Time | `ferroly::rt::time` | `sleep`, `sleep_until`, `timeout`, `timeout_at`, `interval`, `interval_at`, `Duration`, `Instant`, `Interval`, `MissedTickBehavior`, `Sleep`, `Timeout`, `Elapsed` |
| Sync | `ferroly::rt::sync` | `mpsc`, `oneshot`, `broadcast`, `watch`, `Mutex`, `RwLock`, `Semaphore`, `Notify`, `OnceCell`, `Barrier`, their guards and permits (`MutexGuard`, `OwnedMutexGuard`, `RwLockReadGuard`, `OwnedRwLockReadGuard`, `SemaphorePermit`, `OwnedSemaphorePermit`, …), `BarrierWaitResult`, and the error types `AcquireError`, `TryAcquireError`, `TryLockError`, `SetError` |
| Net | `ferroly::rt::net` | `TcpListener`, `TcpStream`, `TcpSocket`, `UdpSocket`, `lookup_host`; on Unix `UnixListener`, `UnixStream`, `UnixDatagram` |
| I/O | `ferroly::rt::io` | `AsyncRead`/`AsyncWrite`/`AsyncBufRead`/`AsyncSeek` + their `…Ext` traits, `BufReader`, `BufWriter`, `ReadBuf`, `copy`, `copy_bidirectional`, `split` (`ReadHalf`, `WriteHalf`), `duplex` (`DuplexStream`), `stdin`, `stdout`, `stderr`, `empty`, `sink` |
| Files | `ferroly::rt::fs` | `File`, `OpenOptions`, `read`, `read_to_string`, `write`, `copy`, `rename`, `remove_file`, `create_dir`, `create_dir_all`, `remove_dir`, `remove_dir_all`, `read_dir` (`ReadDir`, `DirEntry`), `metadata`, `symlink_metadata`, `canonicalize`, `try_exists`, `hard_link`, `read_link`, `set_permissions` |
| Processes | `ferroly::rt::process` | `Command`, `Child`, `ChildStdin`, `ChildStdout`, `ChildStderr` |
| Signals | `ferroly::rt::signal` | `ctrl_c`; on Unix `signal::unix::{signal, Signal, SignalKind}` |
| Runtime | `ferroly::rt::runtime` | `Builder`, `Runtime`, `Handle`, `EnterGuard`, `TryCurrentError` |
| Shutdown | `ferroly::rt` | `CancellationToken`, `DropGuard`, `TaskTracker`, `TrackerTimeout`, `shutdown_signal`, `ShutdownSignal` |

`ferroly::rt::fs` is the raw tokio file surface. For path-confined or
backend-agnostic file access use [vfs](vfs.md).

### Not re-exported

- **`time::pause`, `time::advance`, `time::resume`** — the paused test clock
  exists only when tokio's `test-util` feature is enabled, and Ferroly does not
  enable it for normal builds. A test suite that wants a paused clock needs its
  own `tokio` dev-dependency with `test-util` (after which
  `#[tokio::test(start_paused = true)]` and `tokio::time::advance` work
  alongside `ferroly::rt`).
- Anything else in tokio that is not in the table above (`tokio::io::Lines`
  types, `tokio::net::tcp` split halves, `tokio::task::Builder`, runtime
  metrics, …).

## CancellationToken

A cloneable, latched "please stop" flag.

```rust
pub struct CancellationToken { /* … */ }   // Clone, Debug, Default, Send + Sync

impl CancellationToken {
    pub fn new() -> Self;
    pub fn child_token(&self) -> CancellationToken;
    pub fn cancel(&self);
    pub fn is_cancelled(&self) -> bool;
    pub async fn cancelled(&self);
    pub async fn cancelled_owned(self);
    pub async fn run_until_cancelled<F: Future>(&self, future: F) -> Option<F::Output>;
    pub fn drop_guard(self) -> DropGuard;
}
```

- **Cooperative.** `cancel()` interrupts nothing by itself; tasks notice by
  awaiting `cancelled()` (usually in a `select!`) or polling `is_cancelled()`.
- **Latched.** Once cancelled, always cancelled: a waiter that arrives late
  returns immediately, and `cancel()` is idempotent.
- **Clones share one flag.** Cancelling any clone cancels them all.
- **Child tokens.** `child_token()` is cancelled when its parent is, but
  cancelling the child leaves the parent and its siblings running — "stop this
  request" inside "stop the server". A child of an already-cancelled token
  starts out cancelled.
- `run_until_cancelled(fut)` returns `Some(output)` if the future finished or
  `None` if cancellation won (the future is dropped at its current `.await`).
- `drop_guard()` cancels the token when the guard is dropped;
  `DropGuard::disarm()` takes the token back without cancelling.

```rust
use ferroly::rt::{self, CancellationToken, time::{sleep, Duration}};

# async fn demo() {
let token = CancellationToken::new();
let worker = rt::spawn({
    let token = token.clone();
    async move {
        loop {
            rt::select! {
                _ = token.cancelled() => break "stopped",
                _ = sleep(Duration::from_millis(50)) => { /* one unit of work */ }
            }
        }
    }
});
token.cancel();
assert_eq!(worker.await.unwrap(), "stopped");
# }
```

## TaskTracker

Counts the tasks spawned through it so that a shutdown path can wait for them.
Unlike a `JoinSet`, it keeps no results or handles — only a count — which suits
a server spawning a task per connection.

```rust
pub struct TaskTracker { /* … */ }   // Clone, Debug, Default, Send + Sync

impl TaskTracker {
    pub fn new() -> Self;
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>;
    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>;
    pub fn track_future<F: Future>(&self, future: F) -> impl Future<Output = F::Output>;
    pub fn close(&self) -> bool;
    pub fn reopen(&self) -> bool;
    pub fn is_closed(&self) -> bool;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub async fn wait(&self);
    pub async fn wait_timeout(&self, timeout: Duration) -> Result<(), TrackerTimeout>;
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), TrackerTimeout>;
}

pub struct TrackerTimeout { pub remaining: usize }   // implements std::error::Error
```

- A task is counted from the moment it is spawned until its future is dropped —
  whether it finished, panicked, or was aborted.
- `wait()` completes when the tracker is **closed and empty**. Closing is what
  makes "empty" final; `wait()` on an open tracker never completes. Closing
  does not stop tasks, and tasks spawned after `close()` are still tracked.
- `wait_timeout` / `shutdown` give up after the timeout and report how many
  tasks were still running. `shutdown(timeout)` is `close()` followed by
  `wait_timeout(timeout)`.
- Nothing here aborts tasks. Tell them to stop with a `CancellationToken`;
  to force stragglers down, abort their `JoinHandle`s or drop the runtime.

## shutdown_signal

```rust
pub async fn shutdown_signal() -> std::io::Result<ShutdownSignal>;

#[non_exhaustive]
pub enum ShutdownSignal {
    Interrupt,   // Ctrl-C (SIGINT on Unix)
    Terminate,   // SIGTERM (Unix only)
}
```

Completes when the process is asked to stop: Ctrl-C on every platform, or
`SIGTERM` on Unix (what `kill`, systemd, Docker and Kubernetes send). It
returns an error only if a signal handler could not be installed.

Things to know:

- Handlers are installed when the future is first polled, and only signals
  delivered after that are observed — start awaiting it early.
- Once a handler is installed, the signal's default action (terminating the
  process) no longer happens. The program must exit by itself; a second Ctrl-C
  will not kill a process stuck in its shutdown path unless you await
  `shutdown_signal()` again and exit.
- On Windows only Ctrl-C is observed; console-close and service-stop events are
  not.

## Graceful shutdown

The three helpers compose into the usual pattern — wait for a signal, tell
tasks to stop, give them a bounded time to drain:

```rust
use ferroly::rt::{self, CancellationToken, TaskTracker, time::Duration};

# async fn handle_connection(_stop: CancellationToken) {}
# async fn demo() -> std::io::Result<()> {
let stop = CancellationToken::new();
let tasks = TaskTracker::new();

// Each unit of work gets a child token and is tracked.
for _ in 0..4 {
    tasks.spawn(handle_connection(stop.child_token()));
}

let signal = rt::shutdown_signal().await?;
eprintln!("received {signal:?}, shutting down");

stop.cancel();
if let Err(e) = tasks.shutdown(Duration::from_secs(30)).await {
    eprintln!("{e}");   // "timed out waiting for tasks: N still running"
}
# Ok(())
# }
```

For lower-level control, `signal::ctrl_c()` (cross-platform) and, on Unix,
`signal::unix::signal(SignalKind::terminate())` remain available.

## Cancellation on drop

A spawned task is represented by a `JoinHandle`; dropping it detaches the task,
and `JoinHandle::abort()` (or an `AbortHandle`) cancels it at its next
`.await`:

```rust
use ferroly::rt::{self, time::{sleep, Duration}};

# async fn demo() {
let task = rt::spawn(async {
    loop { sleep(Duration::from_millis(50)).await; }
});
// … later …
task.abort();           // cancel the background loop
# }
```

## Scope

This is a **convenience surface, not a wall**. If you need a tokio primitive
that isn't re-exported here, add `tokio` directly — nothing in `rt` prevents
mixing the two, and the re-exported items are the very same types. The
re-exports carry `#[doc(no_inline)]`, so their canonical docs remain tokio's.

## See also

- [http](http.md) — the server/client that these primitives sit under.
- [lifecycle](lifecycle.md) — component start/stop built on the same runtime.
- [vfs](vfs.md) — confined, backend-agnostic file access.

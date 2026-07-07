# `ferroly::rt` — async runtime surface

Ferroly depends on `tokio` internally. The `rt` module re-exports a curated
slice of it so a consumer can spawn tasks, use channels, and drive timers
**without adding `tokio` to their own `Cargo.toml`** — keeping the
single-dependency promise intact.

Enable with the `rt` feature (it pulls in `tokio`):

```toml
ferroly = { version = "0.1", features = ["rt"] }
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

## What's re-exported

| Area | Path | Items |
|---|---|---|
| Tasks | `ferroly::rt` | `spawn`, `spawn_blocking`, `yield_now`, `JoinHandle`, `JoinSet`, `JoinError` |
| Macros | `ferroly::rt` | `select!`, `join!`, `try_join!`, `pin!` |
| Time | `ferroly::rt::time` | `sleep`, `sleep_until`, `timeout`, `timeout_at`, `interval`, `Duration`, `Instant`, … |
| Sync | `ferroly::rt::sync` | `mpsc`, `oneshot`, `broadcast`, `watch`, `Mutex`, `RwLock`, `Semaphore`, `Notify` |
| Net | `ferroly::rt::net` | `TcpListener`, `TcpStream` |
| I/O | `ferroly::rt::io` | `AsyncRead`/`AsyncWrite` + `…Ext` traits, `BufReader`, `BufWriter`, `copy` |

## Cancellation on drop

A spawned task is represented by a `JoinHandle`; dropping it detaches the task,
and `JoinHandle::abort()` cancels it. For structured cancellation, hold the
handle (or a `JoinSet`) and abort on scope exit:

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
that isn't re-exported here, enable `tokio` directly — nothing in `rt` prevents
mixing the two. The re-exports carry `#[doc(no_inline)]`, so their canonical
docs remain tokio's.

## See also

- [http](http.md) — the server/client that these primitives sit under.
- [lifecycle](lifecycle.md) — component start/stop built on the same runtime.

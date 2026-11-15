//! A small, runtime-agnostic blocking pool: runs a closure on a worker thread
//! and hands its result back through a future, so a long scan never holds an
//! async executor thread.
//!
//! The `vectorstore` feature does not depend on any async runtime, so this is
//! a few worker threads fed by a channel rather than tokio's `spawn_blocking`.
//!
//! The pool sheds load in two ways. Dropping an [`Offloaded`] future before
//! its job starts cancels the job and frees what it captured (a caller that
//! timed out does not leave a scan behind for every other caller to wait
//! on). And at most [`MAX_PENDING`] jobs are queued or running at once; past
//! that, [`run`] refuses new work instead of growing the queue.

use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;
type Outcome<T> = thread::Result<T>;

/// The most worker threads the pool starts.
const MAX_WORKERS: usize = 4;

/// The most jobs that may be queued or running at once, across the process.
/// A cancelled job stops counting as soon as its future is dropped.
pub(crate) const MAX_PENDING: usize = 256;

/// Jobs accepted and neither finished nor cancelled.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// The shared pool, started on first use. `None` when no worker thread could
/// be spawned; work then runs inline.
fn pool() -> Option<&'static Sender<Job>> {
    static POOL: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let workers = thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, MAX_WORKERS);
        let mut started = 0;
        for i in 0..workers {
            let rx = rx.clone();
            let spawned = thread::Builder::new()
                .name(format!("ferroly-vectorstore-{i}"))
                .spawn(move || worker(&rx));
            if spawned.is_ok() {
                started += 1;
            }
        }
        (started > 0).then_some(tx)
    })
    .as_ref()
}

fn worker(rx: &Mutex<Receiver<Job>>) {
    loop {
        // Hold the receiver lock only while waiting for the next job.
        let job = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
            Ok(job) => job,
            Err(_) => return,
        };
        job();
    }
}

/// Where a job is in its life.
enum State<F, T> {
    /// Waiting for a worker; holds the closure so cancelling frees it.
    Queued(F),
    /// A worker is running it.
    Running,
    /// Finished; the outcome waits for the future.
    Done(Outcome<T>),
    /// The future was dropped before the job started, or took the outcome.
    Gone,
}

struct Slot<F, T> {
    state: State<F, T>,
    waker: Option<Waker>,
}

fn lock<F, T>(slot: &Mutex<Slot<F, T>>) -> std::sync::MutexGuard<'_, Slot<F, T>> {
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pool is saturated: [`MAX_PENDING`] jobs are queued or running.
#[derive(Debug)]
pub(crate) struct Saturated;

/// The result of [`run`]: resolves once the job has finished. A panic in the
/// job is re-raised in the task that awaits it. Dropping it before the job
/// starts cancels the job.
pub(crate) struct Offloaded<F, T> {
    slot: Arc<Mutex<Slot<F, T>>>,
}

impl<F, T> Future for Offloaded<F, T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slot = lock(&self.slot);
        match std::mem::replace(&mut slot.state, State::Gone) {
            State::Done(Ok(value)) => Poll::Ready(value),
            State::Done(Err(payload)) => {
                drop(slot);
                panic::resume_unwind(payload)
            }
            other => {
                slot.state = other;
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<F, T> Drop for Offloaded<F, T> {
    fn drop(&mut self) {
        let cancelled = {
            let mut slot = lock(&self.slot);
            match slot.state {
                State::Queued(_) => std::mem::replace(&mut slot.state, State::Gone),
                // A running job cannot be stopped; it releases its place
                // when it ends.
                _ => return,
            }
        };
        PENDING.fetch_sub(1, Ordering::AcqRel);
        // The closure (and what it captured) is dropped here, outside the
        // lock; the queue keeps only a small stub that a worker skips.
        drop(cancelled);
    }
}

/// Runs `job` on a pool thread. If the pool cannot run it (no worker could
/// be started), `job` runs inline, before this returns. Refused with
/// [`Saturated`] when [`MAX_PENDING`] jobs are already queued or running.
pub(crate) fn run<T, F>(job: F) -> Result<Offloaded<F, T>, Saturated>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    run_bounded(job, MAX_PENDING)
}

/// [`run`], refusing the job once `limit` jobs are pending.
fn run_bounded<T, F>(job: F, limit: usize) -> Result<Offloaded<F, T>, Saturated>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // Reserve a place (a compare-exchange loop: `fetch_update` is deprecated
    // on newer toolchains and its replacement is not on the MSRV).
    let mut pending = PENDING.load(Ordering::Acquire);
    loop {
        if pending >= limit {
            return Err(Saturated);
        }
        match PENDING.compare_exchange_weak(
            pending,
            pending + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(now) => pending = now,
        }
    }
    let slot = Arc::new(Mutex::new(Slot {
        state: State::Queued(job),
        waker: None,
    }));
    let shared = slot.clone();
    let wrapped: Job = Box::new(move || {
        let job = {
            let mut slot = lock(&shared);
            match std::mem::replace(&mut slot.state, State::Running) {
                State::Queued(job) => job,
                // Cancelled while queued: nothing to run, and the place was
                // already released.
                other => {
                    slot.state = other;
                    return;
                }
            }
        };
        let outcome = panic::catch_unwind(AssertUnwindSafe(job));
        let waker = {
            let mut slot = lock(&shared);
            slot.state = State::Done(outcome);
            slot.waker.take()
        };
        PENDING.fetch_sub(1, Ordering::AcqRel);
        if let Some(waker) = waker {
            waker.wake();
        }
    });
    match pool() {
        Some(tx) => {
            if let Err(mpsc::SendError(job)) = tx.send(wrapped) {
                job();
            }
        }
        None => wrapped(),
    }
    Ok(Offloaded { slot })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;
    use std::task::Wake;
    use std::time::{Duration, Instant};

    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    fn block_on<Fut: Future>(fut: Fut) -> Fut::Output {
        let mut fut = Box::pin(fut);
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            thread::park_timeout(Duration::from_millis(10));
        }
    }

    /// A job that holds a worker busy.
    type BusyJob = Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Holds every worker busy until the returned sender is dropped or sent
    /// to.
    fn occupy_workers() -> (Vec<BusyJob>, Vec<mpsc::Sender<()>>) {
        let mut futures: Vec<BusyJob> = Vec::new();
        let mut releases = Vec::new();
        let (started_tx, started_rx) = channel();
        for _ in 0..MAX_WORKERS {
            let (tx, rx) = channel::<()>();
            let started = started_tx.clone();
            futures.push(Box::pin(
                run(move || {
                    let _ = started.send(());
                    let _ = rx.recv_timeout(Duration::from_secs(10));
                })
                .unwrap(),
            ));
            releases.push(tx);
        }
        // Wait until every worker the pool has is running one of them.
        let workers = thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, MAX_WORKERS);
        for _ in 0..workers {
            started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        (futures, releases)
    }

    /// A job whose future was dropped before it started never runs, and a
    /// later job does not wait behind the abandoned ones. The pool is global,
    /// so this is the one test that fills it.
    #[test]
    fn dropped_jobs_are_cancelled_and_the_queue_is_bounded() {
        let (busy, releases) = occupy_workers();
        let ran = Arc::new(AtomicBool::new(false));
        let mut accepted = 0;
        let mut refused = 0;
        for _ in 0..(MAX_PENDING * 4) {
            let ran = ran.clone();
            match run(move || {
                ran.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
            }) {
                Ok(fut) => {
                    accepted += 1;
                    drop(fut);
                }
                Err(Saturated) => refused += 1,
            }
        }
        // Each dropped job released its place, so none was refused.
        assert_eq!(refused, 0);
        assert_eq!(accepted, MAX_PENDING * 4);

        // Live (not dropped) jobs are bounded. A small limit keeps other
        // tests that share the global pool clear of the real one.
        let limit = MAX_WORKERS + 8;
        let mut live = Vec::new();
        while live.len() <= limit {
            match run_bounded(|| (), limit) {
                Ok(fut) => live.push(fut),
                Err(Saturated) => break,
            }
        }
        assert!(
            live.len() <= limit - MAX_WORKERS,
            "the pool accepted more than {limit} jobs"
        );
        drop(live);

        drop(releases);
        for fut in busy {
            block_on(fut);
        }
        let started = Instant::now();
        block_on(run(|| ()).unwrap());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a fresh job waited {:?} behind abandoned ones",
            started.elapsed()
        );
        assert!(!ran.load(Ordering::SeqCst), "a dropped job ran");
    }
}
